//! The sidebar's keymap: which keys run which sidebar actions.
//!
//! [`DEFAULT_KEYS`] is what the sidebar does out of the box. `[keys]` and
//! `[sidebar.keys]` in `~/.config/pmux/config.toml` rebind any action; see
//! [`crate::keys`] for how they combine.

use super::*;
use crate::keys::{DefaultKeys, KeyAction, KeyLookup, KeyTables, Keymap};
pub(super) use crate::keys::{KeyChord, PendingKeys};

/// Something a key can do in the sidebar. The names are the keys of the
/// `[sidebar.keys]` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SidebarAction {
    MoveUp,
    MoveDown,
    HalfPageUp,
    HalfPageDown,
    PageUp,
    PageDown,
    First,
    Last,
    SelectHeader,
    Toggle,
    Open,
    Menu,
    FoldSection,
    CycleGrouping,
    CycleMetric,
    CycleScale,
    Search,
    Close,
    Help,
}

impl KeyAction for SidebarAction {
    const ALL: &'static [Self] = &[
        Self::MoveUp,
        Self::MoveDown,
        Self::HalfPageUp,
        Self::HalfPageDown,
        Self::PageUp,
        Self::PageDown,
        Self::First,
        Self::Last,
        Self::SelectHeader,
        Self::Toggle,
        Self::Open,
        Self::Menu,
        Self::FoldSection,
        Self::CycleGrouping,
        Self::CycleMetric,
        Self::CycleScale,
        Self::Search,
        Self::Close,
        Self::Help,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::MoveUp => "move_up",
            Self::MoveDown => "move_down",
            Self::HalfPageUp => "half_page_up",
            Self::HalfPageDown => "half_page_down",
            Self::PageUp => "page_up",
            Self::PageDown => "page_down",
            Self::First => "first",
            Self::Last => "last",
            Self::SelectHeader => "select_header",
            Self::Toggle => "toggle",
            Self::Open => "open",
            Self::Menu => "menu",
            Self::FoldSection => "fold_section",
            Self::CycleGrouping => "cycle_grouping",
            Self::CycleMetric => "cycle_metric",
            Self::CycleScale => "cycle_scale",
            Self::Search => "search",
            Self::Close => "close",
            Self::Help => "help",
        }
    }
}

/// Each action's default keys: those the keymap help shows, then those that
/// work but are left out of it to keep the help to the Vim-style keys.
/// `menu` has none here: it defaults to the arrow pointing into the main
/// panes, which depends on the sidebar's side.
const DEFAULT_KEYS: &DefaultKeys<SidebarAction> = &[
    (SidebarAction::MoveUp, &["k", "up"], &[]),
    (SidebarAction::MoveDown, &["j", "down"], &[]),
    (SidebarAction::HalfPageUp, &["C-u"], &[]),
    (SidebarAction::HalfPageDown, &["C-d"], &[]),
    (SidebarAction::PageUp, &[], &["pageup"]),
    (SidebarAction::PageDown, &[], &["pagedown"]),
    (SidebarAction::First, &["gg"], &[]),
    (SidebarAction::Last, &["G"], &["end"]),
    (SidebarAction::SelectHeader, &[], &["home"]),
    (SidebarAction::Toggle, &["enter"], &[]),
    (SidebarAction::Open, &["tab"], &[]),
    (SidebarAction::FoldSection, &["z"], &[]),
    (SidebarAction::CycleGrouping, &["a"], &[]),
    (SidebarAction::CycleMetric, &["m"], &[]),
    (SidebarAction::CycleScale, &["x"], &[]),
    (SidebarAction::Search, &["/"], &[]),
    (SidebarAction::Close, &["q", "esc"], &[]),
    (SidebarAction::Help, &["?"], &[]),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SidebarKeymap {
    keymap: Keymap<SidebarAction>,
}

impl Default for SidebarKeymap {
    fn default() -> Self {
        Self::from_tables(&KeyTables::default()).0
    }
}

impl SidebarKeymap {
    /// The keymap from `[keys]` and `[sidebar.keys]` in config.toml, and a
    /// message for each entry that could not be used. A missing file or
    /// table gives the defaults.
    pub(super) fn load() -> (Self, Vec<String>) {
        let (tables, mut warnings) = KeyTables::load("sidebar");
        let (keymap, more) = Self::from_tables(&tables);
        warnings.extend(more);
        (keymap, warnings)
    }

    #[cfg(test)]
    pub(super) fn from_config_text(text: &str) -> (Self, Vec<String>) {
        let (tables, mut warnings) = KeyTables::from_config_text(text, "sidebar");
        let (keymap, more) = Self::from_tables(&tables);
        warnings.extend(more);
        (keymap, warnings)
    }

    fn from_tables(tables: &KeyTables) -> (Self, Vec<String>) {
        let (keymap, warnings) = Keymap::build("sidebar", tables, DEFAULT_KEYS, |_| Ok(()));
        (Self { keymap }, warnings)
    }

    fn inward_arrow(sidebar_on_right: bool) -> KeyChord {
        KeyChord::plain(if sidebar_on_right {
            KeyCode::Left
        } else {
            KeyCode::Right
        })
    }

    /// The default menu arrow (pointing into the main panes), unless the
    /// user set `menu` or a binding has taken that key.
    fn inward_menu_key(&self, sidebar_on_right: bool) -> Option<KeyChord> {
        let arrow = Self::inward_arrow(sidebar_on_right);
        (!self.keymap.is_configured(SidebarAction::Menu) && !self.keymap.binds_first(arrow))
            .then_some(arrow)
    }

    pub(super) fn lookup(
        &self,
        pressed: &[KeyChord],
        sidebar_on_right: bool,
    ) -> KeyLookup<SidebarAction> {
        let lookup = self.keymap.lookup(pressed);
        if lookup == KeyLookup::Unbound
            && pressed.len() == 1
            && self.inward_menu_key(sidebar_on_right) == Some(pressed[0])
        {
            return KeyLookup::Action(SidebarAction::Menu);
        }
        lookup
    }

    /// The keys the help shows for `action`, such as `k/↑`, or `None` when
    /// it has none.
    pub(super) fn label(&self, action: SidebarAction, sidebar_on_right: bool) -> Option<String> {
        let mut labels = self.keymap.labels(action);
        if action == SidebarAction::Menu
            && let Some(arrow) = self.inward_menu_key(sidebar_on_right)
        {
            labels.push(arrow.label());
        }
        (!labels.is_empty()).then(|| labels.join("/"))
    }
}

impl SidebarApp {
    /// Add a key press to any pending sequence and return the action it
    /// completes.
    pub(super) fn resolve_key(
        &mut self,
        key: KeyEvent,
        sidebar_on_right: bool,
    ) -> Option<SidebarAction> {
        let keymap = &self.keymap;
        match self
            .pending_keys
            .resolve(KeyChord::from_event(key), |pressed| {
                keymap.lookup(pressed, sidebar_on_right)
            }) {
            KeyLookup::Action(action) => Some(action),
            KeyLookup::Pending | KeyLookup::Unbound => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::parse_binding;

    fn keys(text: &str) -> Vec<KeyChord> {
        parse_binding(text).unwrap()
    }

    fn action(keymap: &SidebarKeymap, text: &str) -> KeyLookup<SidebarAction> {
        keymap.lookup(&keys(text), false)
    }

    #[test]
    fn the_defaults_bind_vim_keys_and_quietly_keep_the_others() {
        let keymap = SidebarKeymap::default();
        assert_eq!(
            action(&keymap, "j"),
            KeyLookup::Action(SidebarAction::MoveDown)
        );
        assert_eq!(action(&keymap, "g"), KeyLookup::Pending);
        assert_eq!(
            action(&keymap, "gg"),
            KeyLookup::Action(SidebarAction::First)
        );
        assert_eq!(
            action(&keymap, "end"),
            KeyLookup::Action(SidebarAction::Last)
        );
        assert_eq!(
            action(&keymap, "z"),
            KeyLookup::Action(SidebarAction::FoldSection)
        );
        assert_eq!(
            keymap.label(SidebarAction::Last, false).as_deref(),
            Some("G")
        );
        assert_eq!(keymap.label(SidebarAction::PageDown, false), None);
        assert_eq!(
            keymap.label(SidebarAction::MoveUp, false).as_deref(),
            Some("k/↑")
        );
        assert_eq!(
            keymap.label(SidebarAction::HalfPageDown, false).as_deref(),
            Some("^d")
        );
    }

    #[test]
    fn the_menu_arrow_points_into_the_main_panes() {
        let keymap = SidebarKeymap::default();
        let right = [KeyChord::plain(KeyCode::Right)];
        let left = [KeyChord::plain(KeyCode::Left)];
        assert_eq!(
            keymap.lookup(&right, false),
            KeyLookup::Action(SidebarAction::Menu)
        );
        assert_eq!(keymap.lookup(&left, false), KeyLookup::Unbound);
        assert_eq!(
            keymap.lookup(&left, true),
            KeyLookup::Action(SidebarAction::Menu)
        );
        assert_eq!(
            keymap.label(SidebarAction::Menu, true).as_deref(),
            Some("←")
        );
    }

    #[test]
    fn overrides_replace_an_actions_keys_and_take_them_from_the_defaults() {
        let (keymap, warnings) = SidebarKeymap::from_config_text(
            r#"
            [sidebar.keys]
            fold_section = ["za", "space"]
            close = "Q"
            menu = ["l", "right"]
            "#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(action(&keymap, "z"), KeyLookup::Pending);
        assert_eq!(
            action(&keymap, "za"),
            KeyLookup::Action(SidebarAction::FoldSection)
        );
        assert_eq!(action(&keymap, "q"), KeyLookup::Unbound);
        assert_eq!(action(&keymap, "esc"), KeyLookup::Unbound);
        assert_eq!(
            action(&keymap, "Q"),
            KeyLookup::Action(SidebarAction::Close)
        );
        assert_eq!(action(&keymap, "l"), KeyLookup::Action(SidebarAction::Menu));
        // Untouched actions keep their defaults.
        assert_eq!(
            action(&keymap, "j"),
            KeyLookup::Action(SidebarAction::MoveDown)
        );
        assert_eq!(
            keymap.label(SidebarAction::Menu, true).as_deref(),
            Some("l/→"),
            "a configured menu no longer follows the sidebar's side"
        );
    }

    #[test]
    fn a_key_given_to_another_action_leaves_its_default() {
        let (keymap, warnings) =
            SidebarKeymap::from_config_text("[sidebar.keys]\ncycle_metric = \"a\"\nhelp = \"g\"\n");
        assert!(warnings.is_empty());
        assert_eq!(
            action(&keymap, "a"),
            KeyLookup::Action(SidebarAction::CycleMetric)
        );
        assert_eq!(keymap.label(SidebarAction::CycleGrouping, false), None);
        // `g` alone now opens the help, so the default `gg` is dropped.
        assert_eq!(action(&keymap, "g"), KeyLookup::Action(SidebarAction::Help));
        assert_eq!(keymap.label(SidebarAction::First, false), None);
    }

    #[test]
    fn an_empty_list_unbinds_and_bad_entries_are_reported() {
        let (keymap, warnings) = SidebarKeymap::from_config_text(
            r#"
            [sidebar.keys]
            cycle_scale = []
            jump = "J"
            help = ["<bogus>", "?"]
            toggle = 3
            "#,
        );
        assert_eq!(action(&keymap, "x"), KeyLookup::Unbound);
        assert_eq!(action(&keymap, "?"), KeyLookup::Action(SidebarAction::Help));
        assert_eq!(
            action(&keymap, "enter"),
            KeyLookup::Action(SidebarAction::Toggle),
            "an unusable entry keeps the defaults"
        );
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("unknown action `jump`")));
        assert!(warnings.iter().any(|w| w.contains("`<bogus>`")));

        let (keymap, warnings) = SidebarKeymap::from_config_text("not toml [");
        assert_eq!(keymap, SidebarKeymap::default());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn shared_keys_apply_to_the_sidebar_and_its_own_table_wins() {
        let (keymap, warnings) = SidebarKeymap::from_config_text(
            r#"
            [keys]
            move_down = ["n", "C-j"]
            delete_char = "C-h"  # a picker action
            close = "Q"
            [sidebar.keys]
            close = "esc"
            "#,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            action(&keymap, "n"),
            KeyLookup::Action(SidebarAction::MoveDown)
        );
        assert_eq!(action(&keymap, "j"), KeyLookup::Unbound);
        assert_eq!(action(&keymap, "Q"), KeyLookup::Unbound);
        assert_eq!(
            action(&keymap, "esc"),
            KeyLookup::Action(SidebarAction::Close)
        );
    }

    #[test]
    fn a_pending_sequence_falls_back_to_the_latest_key() {
        let mut app = SidebarApp::new(None);
        let press = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
        assert_eq!(app.resolve_key(press('g'), false), None);
        assert_eq!(
            app.resolve_key(press('j'), false),
            Some(SidebarAction::MoveDown)
        );
        assert!(app.pending_keys.is_empty());
        assert_eq!(app.resolve_key(press('g'), false), None);
        assert_eq!(
            app.resolve_key(press('g'), false),
            Some(SidebarAction::First)
        );
        let shift_g = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(app.resolve_key(shift_g, false), Some(SidebarAction::Last));
    }

    #[test]
    fn the_customization_docs_key_examples_apply_cleanly() {
        let doc = include_str!("../../docs/sidebar-customization.md");
        let blocks: Vec<&str> = doc
            .split("```toml\n")
            .skip(1)
            .filter_map(|part| part.split_once("```").map(|(block, _)| block))
            .filter(|block| block.contains("[sidebar.keys]") || block.contains("[keys]"))
            .collect();
        assert!(!blocks.is_empty());
        for block in blocks {
            let (_, warnings) = SidebarKeymap::from_config_text(block);
            assert!(warnings.is_empty(), "{block}: {warnings:?}");
        }
    }
}
