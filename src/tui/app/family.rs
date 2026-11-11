//! Lineage in the sidebar: a process spawned by another listed process is
//! drawn under it, and its family is ordered as one entry.
//!
//! The list runs waiting, then working, then the rest. A family takes the
//! place of its most pressing member, so a working child lifts its idle
//! parent's family into the working rows. A child waiting on the user leaves
//! its family for the top of the list, as its own row naming its parent,
//! and goes back under it once it stops waiting. A child whose parent is on
//! another machine cannot nest, and names its parent the same way.

use super::*;

/// Generations drawn under a root. A process deeper than this is drawn at
/// the last of them, under the ancestor there is room for, naming its own
/// parent, so indentation never eats a row's name at the sidebar's width.
pub(in crate::tui) const MAX_TREE_DEPTH: usize = 3;

/// How the sidebar's processes are related, by index into
/// [`SidebarApp::processes`].
pub(super) struct Lineage {
    parents: Vec<Option<usize>>,
    /// Each process's children drawn under it, in list order.
    children: Vec<Vec<usize>>,
    /// The parent a row names because it is not drawn under it.
    notes: Vec<Option<String>>,
    /// Processes a search left out of the list.
    hidden: Vec<bool>,
}

impl Lineage {
    /// The processes listed in their own right: no parent drawn above them.
    pub(super) fn roots(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.parents.len())
            .filter(|index| self.parents[*index].is_none() && !self.hidden[*index])
    }

    /// Leave out every process that is not in `matched` and has nothing
    /// under it that is, so a match keeps the ancestors it is drawn under.
    pub(super) fn retain_matches(&mut self, matched: &[bool]) {
        let mut keep = vec![false; matched.len()];
        for index in (0..matched.len()).filter(|index| matched[*index]) {
            let mut at = Some(index);
            while let Some(current) = at {
                if keep[current] {
                    break;
                }
                keep[current] = true;
                at = self.parents[current];
            }
        }
        for children in &mut self.children {
            children.retain(|child| keep[*child]);
        }
        self.hidden = keep.iter().map(|kept| !kept).collect();
    }

    /// `root` and every process drawn under it, depth first in list order.
    pub(super) fn family(&self, root: usize) -> Vec<usize> {
        let mut family = vec![root];
        let mut next = 0;
        while next < family.len() {
            let children = &self.children[family[next]];
            family.splice(next + 1..next + 1, children.iter().copied());
            next += 1;
        }
        family
    }
}

/// How pressing a process's state is: waiting on the user, then working,
/// then the rest.
pub(super) fn attention_tier(process: &crate::ProcessInfo) -> u8 {
    match process
        .agent
        .as_ref()
        .and_then(|agent| agent.state.as_deref())
    {
        Some("waiting" | "needs_input" | "attention") => 0,
        Some("working") => 1,
        _ => 2,
    }
}

impl SidebarApp {
    /// The local process that spawned `index`, if it is listed.
    fn listed_parent(&self, index: usize) -> Option<usize> {
        let spawned_by = self.processes[index].process.spawned_by.as_ref()?;
        if spawned_by.is_remote() {
            return None;
        }
        self.processes
            .iter()
            .position(|candidate| {
                candidate.process.name == spawned_by.process
                    && spawned_by
                        .created_at
                        .is_none_or(|created_at| created_at == candidate.process.created_at)
            })
            .filter(|parent| *parent != index)
    }

    /// Who is drawn under whom: each process under its listed local parent,
    /// unless it waits on the user, its parent is not listed, the records
    /// loop, or it is deeper than [`MAX_TREE_DEPTH`].
    pub(super) fn lineage(&self) -> Lineage {
        let count = self.processes.len();
        let mut parents: Vec<Option<usize>> = (0..count)
            .map(|index| {
                self.listed_parent(index)
                    .filter(|_| attention_tier(&self.processes[index].process) != 0)
            })
            .collect();
        // A loop has no root to hang from: each of its members stands alone.
        let looped: Vec<usize> = (0..count)
            .filter(|start| {
                let mut at = parents[*start];
                for _ in 0..count {
                    match at {
                        Some(index) if index == *start => return true,
                        Some(index) => at = parents[index],
                        None => return false,
                    }
                }
                true
            })
            .collect();
        for index in looped {
            parents[index] = None;
        }
        let ancestors = |index: usize| {
            std::iter::successors(parents[index], |at| parents[*at]).collect::<Vec<_>>()
        };
        let capped: Vec<Option<usize>> = (0..count)
            .map(|index| {
                let ancestors = ancestors(index);
                if ancestors.len() > MAX_TREE_DEPTH {
                    // The ancestor at depth MAX_TREE_DEPTH - 1, counted from the root.
                    Some(ancestors[ancestors.len() - MAX_TREE_DEPTH])
                } else {
                    parents[index]
                }
            })
            .collect();
        let notes = (0..count)
            .map(|index| {
                let spawned_by = self.processes[index].process.spawned_by.as_ref()?;
                let drawn_under =
                    capped[index].is_some_and(|parent| Some(parent) == parents[index]);
                (!drawn_under).then(|| spawned_by.describe())
            })
            .collect();
        let mut children = vec![Vec::new(); count];
        for (index, parent) in capped.iter().enumerate() {
            if let Some(parent) = parent {
                children[*parent].push(index);
            }
        }
        for siblings in &mut children {
            siblings.sort_by(|left, right| self.list_order(*left, *right));
        }
        Lineage {
            parents: capped,
            children,
            notes,
            hidden: vec![false; count],
        }
    }

    /// The order of two families by their roots: the most pressing member's
    /// tier, then when the latest of that tier changed course, newest first;
    /// then as [`Self::list_order`] orders the roots.
    pub(super) fn family_order(
        &self,
        lineage: &Lineage,
        left: usize,
        right: usize,
    ) -> std::cmp::Ordering {
        let rank = |root: usize| {
            lineage
                .family(root)
                .into_iter()
                .map(|index| {
                    let process = &self.processes[index].process;
                    (
                        attention_tier(process),
                        std::cmp::Reverse(self.course_since(process)),
                    )
                })
                .min()
                .expect("a family has its root")
        };
        rank(left)
            .cmp(&rank(right))
            .then_with(|| self.list_order(left, right))
    }

    /// The rows of `root`'s family, as listed under the group `prefix`,
    /// `root` standing for `location`.
    pub(super) fn family_rows(
        &self,
        lineage: &Lineage,
        root: usize,
        prefix: &str,
        location: Option<crate::OuterPaneLocation>,
    ) -> Vec<SidebarListRow> {
        let mut rows = Vec::new();
        self.push_family_row(
            &mut rows,
            lineage,
            root,
            prefix,
            location,
            render::RowTree::default(),
        );
        rows
    }

    fn push_family_row(
        &self,
        rows: &mut Vec<SidebarListRow>,
        lineage: &Lineage,
        index: usize,
        prefix: &str,
        location: Option<crate::OuterPaneLocation>,
        tree: render::RowTree,
    ) {
        let children = &lineage.children[index];
        let mut guides = tree.guides.clone();
        if tree.depth > 0 {
            guides.push(!tree.last);
        }
        let depth = tree.depth + 1;
        rows.push(SidebarListRow::Process {
            process_index: index,
            identity: format!("{prefix}:process:{}", self.processes[index].process.name),
            location,
            tree: render::RowTree {
                has_children: !children.is_empty(),
                ..tree
            },
            parent_note: lineage.notes[index].clone(),
        });
        for (position, child) in children.iter().enumerate() {
            self.push_family_row(
                rows,
                lineage,
                *child,
                prefix,
                None,
                render::RowTree {
                    depth,
                    guides: guides.clone(),
                    last: position + 1 == children.len(),
                    has_children: false,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::*;

    /// A claude process `name` in `state` since `updated_at`, started at
    /// `created_at`, spawned by `parent` on this machine.
    fn agent(
        name: &str,
        created_at: i64,
        state: &str,
        updated_at: i64,
        parent: Option<(&str, i64)>,
    ) -> crate::SidebarProcess {
        let mut process = test_process(Some("/work"), None);
        process.process.name = name.to_string();
        process.process.pane_id = format!("%{name}");
        process.process.created_at = created_at;
        process.process.agent = Some(crate::AgentInfo {
            kind: "claude".to_string(),
            state: Some(state.to_string()),
            updated_at: Some(updated_at),
            source: None,
            reason: None,
            session_id: None,
        });
        process.process.spawned_by =
            parent.map(|(parent, created_at)| crate::SpawnedBy::local(parent, created_at));
        process
    }

    fn show(app: &mut SidebarApp, processes: Vec<crate::SidebarProcess>) {
        app.processes = processes;
        app.settle_courses();
        app.rebuild_rows();
    }

    fn ungrouped() -> SidebarApp {
        let mut app = SidebarApp::new(None);
        app.grouping.automatic = crate::AutomaticGrouping::Ungrouped;
        app
    }

    /// Each process row as `name`, indented two spaces per generation, then
    /// ` (from parent)` when it names one.
    fn listing(app: &SidebarApp) -> Vec<String> {
        app.rows
            .iter()
            .filter_map(|row| match row {
                SidebarListRow::Process {
                    process_index,
                    tree,
                    parent_note,
                    ..
                } => Some(format!(
                    "{}{}{}",
                    "  ".repeat(tree.depth),
                    app.processes[*process_index].process.name,
                    parent_note
                        .as_ref()
                        .map(|note| format!(" (from {note})"))
                        .unwrap_or_default()
                )),
                _ => None,
            })
            .collect()
    }

    fn trees(app: &SidebarApp) -> Vec<render::RowTree> {
        app.rows
            .iter()
            .filter_map(|row| match row {
                SidebarListRow::Process { tree, .. } => Some(tree.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn children_are_listed_under_their_parent_in_the_list_order() {
        let mut app = ungrouped();
        show(
            &mut app,
            vec![
                agent("other", 5, "done", 50, None),
                agent("lead", 10, "done", 100, None),
                agent("old-helper", 20, "done", 100, Some(("lead", 10))),
                agent("lint", 25, "done", 100, Some(("old-helper", 20))),
                agent("new-helper", 30, "working", 100, Some(("lead", 10))),
            ],
        );
        // The working child lifts its family above `other`, and lists first
        // under its parent.
        assert_eq!(
            listing(&app),
            ["lead", "  new-helper", "  old-helper", "    lint", "other"]
        );
        let tree = |depth, guides: &[bool], last, has_children| render::RowTree {
            depth,
            guides: guides.to_vec(),
            last,
            has_children,
        };
        assert_eq!(
            trees(&app),
            [
                tree(0, &[], false, true),
                tree(1, &[], false, false),
                tree(1, &[], true, true),
                tree(2, &[false], true, false),
                tree(0, &[], false, false),
            ]
        );
    }

    #[test]
    fn a_waiting_child_goes_to_the_top_and_back_under_its_parent() {
        let mut app = ungrouped();
        let processes = |helper_state: &str, helper_at: i64| {
            vec![
                agent("busy", 5, "working", 100, None),
                agent("lead", 10, "done", 100, None),
                agent("helper", 20, helper_state, helper_at, Some(("lead", 10))),
                agent("tests", 30, "done", 100, Some(("lead", 10))),
            ]
        };
        // At first sight, the newer start (the helper's) lists first.
        show(&mut app, processes("working", 100));
        assert_eq!(listing(&app), ["lead", "  helper", "  tests", "busy"]);

        // Waiting on the user: its own row at the top, naming its parent,
        // above the working agents.
        show(&mut app, processes("waiting", 200));
        assert_eq!(
            listing(&app),
            ["helper (from lead)", "busy", "lead", "  tests"]
        );

        // Answered and working again: back under its parent, which its work
        // lifts to the top of the working rows.
        show(&mut app, processes("working", 300));
        assert_eq!(listing(&app), ["lead", "  helper", "  tests", "busy"]);
    }

    #[test]
    fn the_list_runs_waiting_then_working_then_the_rest_and_stays_steady() {
        let mut app = ungrouped();
        show(
            &mut app,
            vec![
                agent("done", 40, "done", 100, None),
                agent("working", 30, "working", 100, None),
                agent("waiting", 10, "waiting", 100, None),
                agent("also-working", 20, "working", 100, None),
            ],
        );
        assert_eq!(
            listing(&app),
            ["waiting", "working", "also-working", "done"]
        );
        // Signals while working move nothing.
        show(
            &mut app,
            vec![
                agent("done", 40, "done", 100, None),
                agent("working", 30, "working", 500, None),
                agent("waiting", 10, "waiting", 100, None),
                agent("also-working", 20, "working", 600, None),
            ],
        );
        assert_eq!(
            listing(&app),
            ["waiting", "working", "also-working", "done"]
        );
    }

    #[test]
    fn a_child_of_a_remote_or_missing_parent_stands_alone_naming_it() {
        let mut app = ungrouped();
        let mut remote = agent("sidebar", 20, "working", 100, None);
        remote.process.spawned_by = Some(crate::SpawnedBy {
            process: "lineage ios".to_string(),
            created_at: None,
            machine: Some("macbook".to_string()),
            url: Some("http://100.72.15.45:9470".to_string()),
        });
        // A parent of the same name but another session is not this one's.
        let reused = agent("orphan", 10, "done", 100, Some(("lead", 1)));
        show(
            &mut app,
            vec![remote, reused, agent("lead", 5, "done", 50, None)],
        );
        assert_eq!(
            listing(&app),
            [
                "sidebar (from lineage ios on macbook)",
                "orphan (from lead)",
                "lead"
            ]
        );
    }

    #[test]
    fn a_family_stays_in_its_roots_group() {
        let mut app = SidebarApp::new(None);
        app.grouping.automatic = crate::AutomaticGrouping::Cwd;
        let mut helper = agent("helper", 20, "done", 100, Some(("lead", 10)));
        helper.process.current_path = Some("/elsewhere".to_string());
        show(&mut app, vec![agent("lead", 10, "done", 100, None), helper]);
        let groups: Vec<_> = app
            .rows
            .iter()
            .filter_map(|row| match row {
                SidebarListRow::Group(group) => Some((group.key.clone(), group.members.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            groups,
            [(
                "/work".to_string(),
                vec!["lead".to_string(), "helper".to_string()]
            )]
        );
        assert_eq!(listing(&app), ["lead", "  helper"]);
    }

    #[test]
    fn deep_descendants_and_loops_keep_the_tree_readable() {
        let mut app = ungrouped();
        show(
            &mut app,
            vec![
                agent("g0", 10, "done", 100, None),
                agent("g1", 20, "done", 100, Some(("g0", 10))),
                agent("g2", 30, "done", 100, Some(("g1", 20))),
                agent("g3", 40, "done", 100, Some(("g2", 30))),
                agent("g4", 50, "done", 100, Some(("g3", 40))),
            ],
        );
        // Past MAX_TREE_DEPTH a process is drawn at the last depth, naming
        // its own parent.
        assert_eq!(
            listing(&app),
            ["g0", "  g1", "    g2", "      g4 (from g3)", "      g3"]
        );

        show(
            &mut app,
            vec![
                agent("a", 10, "done", 100, Some(("b", 20))),
                agent("b", 20, "done", 100, Some(("a", 10))),
            ],
        );
        assert_eq!(listing(&app), ["b (from a)", "a (from b)"]);
    }
}
