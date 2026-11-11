//! Configurable keymaps shared by the sidebar and the attach picker.
//!
//! Each surface names its actions (a [`KeyAction`]), lists their default
//! keys, and reads a `[<surface>.keys]` table from
//! `~/.config/pmux/config.toml`:
//!
//! ```toml
//! [sidebar.keys]
//! fold_section = ["z", "za"]
//! close = "q"
//! ```
//!
//! An action listed there gets exactly the keys given (an empty list unbinds
//! it). Every other action keeps its defaults, minus any default key the user
//! gave to something else.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::hash::Hash;

/// Something a key can do on one surface. The names are the keys of that
/// surface's `[<surface>.keys]` table.
pub(crate) trait KeyAction: Copy + Eq + Hash + std::fmt::Debug + 'static {
    const ALL: &'static [Self];

    fn name(self) -> &'static str;

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|action| action.name() == name)
    }
}

/// Each action's default keys: those a keymap help shows, then those that
/// work but are left out of it.
pub(crate) type DefaultKeys<A> = [(A, &'static [&'static str], &'static [&'static str])];

/// One key press. Shift is part of the character (`G`), so only Ctrl and Alt
/// are kept beside the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct KeyChord {
    code: KeyCode,
    ctrl: bool,
    alt: bool,
}

impl KeyChord {
    pub(crate) fn plain(code: KeyCode) -> Self {
        Self {
            code,
            ctrl: false,
            alt: false,
        }
    }

    pub(crate) fn from_event(key: KeyEvent) -> Self {
        Self {
            code: key.code,
            ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
            alt: key.modifiers.contains(KeyModifiers::ALT),
        }
    }

    /// The character this press types, if it is a plain printable key.
    pub(crate) fn typed_char(self) -> Option<char> {
        match self.code {
            KeyCode::Char(ch) if !self.ctrl && !self.alt => Some(ch),
            _ => None,
        }
    }

    pub(crate) fn label(self) -> String {
        let base = match self.code {
            KeyCode::Char(' ') => "Space".to_string(),
            KeyCode::Char(ch) => ch.to_string(),
            KeyCode::Up => "↑".to_string(),
            KeyCode::Down => "↓".to_string(),
            KeyCode::Left => "←".to_string(),
            KeyCode::Right => "→".to_string(),
            KeyCode::Enter => "Enter".to_string(),
            KeyCode::Tab => "Tab".to_string(),
            KeyCode::BackTab => "S-Tab".to_string(),
            KeyCode::Esc => "Esc".to_string(),
            KeyCode::PageUp => "PgUp".to_string(),
            KeyCode::PageDown => "PgDn".to_string(),
            KeyCode::Home => "Home".to_string(),
            KeyCode::End => "End".to_string(),
            KeyCode::Backspace => "BS".to_string(),
            KeyCode::Delete => "Del".to_string(),
            KeyCode::Insert => "Ins".to_string(),
            KeyCode::F(number) => format!("F{number}"),
            other => format!("{other:?}"),
        };
        match (self.ctrl, self.alt) {
            (true, true) => format!("^M-{base}"),
            (true, false) => format!("^{base}"),
            (false, true) => format!("M-{base}"),
            (false, false) => base,
        }
    }
}

const NAMED_KEYS: &[(&str, KeyCode)] = &[
    ("enter", KeyCode::Enter),
    ("return", KeyCode::Enter),
    ("cr", KeyCode::Enter),
    ("tab", KeyCode::Tab),
    ("backtab", KeyCode::BackTab),
    ("s-tab", KeyCode::BackTab),
    ("esc", KeyCode::Esc),
    ("escape", KeyCode::Esc),
    ("space", KeyCode::Char(' ')),
    ("up", KeyCode::Up),
    ("down", KeyCode::Down),
    ("left", KeyCode::Left),
    ("right", KeyCode::Right),
    ("pageup", KeyCode::PageUp),
    ("pgup", KeyCode::PageUp),
    ("pagedown", KeyCode::PageDown),
    ("pgdn", KeyCode::PageDown),
    ("home", KeyCode::Home),
    ("end", KeyCode::End),
    ("backspace", KeyCode::Backspace),
    ("bs", KeyCode::Backspace),
    ("delete", KeyCode::Delete),
    ("del", KeyCode::Delete),
    ("insert", KeyCode::Insert),
];

fn named_key(name: &str) -> Option<KeyCode> {
    let name = name.to_ascii_lowercase();
    if let Some(number) = name.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        return (1..=12).contains(&number).then_some(KeyCode::F(number));
    }
    NAMED_KEYS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, code)| *code)
}

/// One token naming a single key: `enter`, `<CR>`, `C-d`, `<C-d>`, `ctrl-d`,
/// `alt-x`. `None` means the token is not that form and is read as a run of
/// characters instead.
fn parse_named_token(token: &str) -> Option<Result<KeyChord, String>> {
    let bracketed = token.len() > 2 && token.starts_with('<') && token.ends_with('>');
    let mut rest = if bracketed {
        &token[1..token.len() - 1]
    } else {
        token
    };
    let mut chord = KeyChord::plain(KeyCode::Null);
    loop {
        let lower = rest.to_ascii_lowercase();
        let prefix = ["ctrl-", "ctrl+", "control-", "c-"]
            .iter()
            .find(|prefix| lower.starts_with(**prefix))
            .map(|prefix| (prefix.len(), true))
            .or_else(|| {
                ["alt-", "alt+", "meta-", "a-", "m-"]
                    .iter()
                    .find(|prefix| lower.starts_with(**prefix))
                    .map(|prefix| (prefix.len(), false))
            });
        match prefix {
            Some((length, ctrl)) if rest.len() > length => {
                if ctrl {
                    chord.ctrl = true;
                } else {
                    chord.alt = true;
                }
                rest = &rest[length..];
            }
            _ => break,
        }
    }
    let modified = chord.ctrl || chord.alt;
    let mut chars = rest.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        if !modified && !bracketed {
            return None;
        }
        // A Ctrl key arrives as its lowercase letter.
        chord.code = KeyCode::Char(if chord.ctrl {
            ch.to_ascii_lowercase()
        } else {
            ch
        });
        return Some(Ok(chord));
    }
    match named_key(rest) {
        Some(code) => {
            chord.code = code;
            Some(Ok(chord))
        }
        None if modified || bracketed => Some(Err(format!("unknown key `{token}`"))),
        None => None,
    }
}

/// A binding's text: whitespace-separated tokens, each a named key or a run
/// of characters typed one after another (`gg`, `z a`, `<C-w> j`).
pub(crate) fn parse_binding(text: &str) -> Result<Vec<KeyChord>, String> {
    let mut keys = Vec::new();
    for token in text.split_whitespace() {
        match parse_named_token(token) {
            Some(chord) => keys.push(chord?),
            None => keys.extend(token.chars().map(|ch| KeyChord::plain(KeyCode::Char(ch)))),
        }
    }
    if keys.is_empty() {
        return Err("empty key".to_string());
    }
    Ok(keys)
}

fn binding_label(keys: &[KeyChord]) -> String {
    let labels: Vec<String> = keys.iter().map(|key| key.label()).collect();
    if labels.iter().all(|label| label.chars().count() == 1) {
        labels.concat()
    } else {
        labels.join(" ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Binding<A> {
    keys: Vec<KeyChord>,
    action: A,
    /// Shown in a keymap help.
    hinted: bool,
}

/// What the keys pressed so far mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyLookup<A> {
    Action(A),
    /// The start of a longer binding (the first `g` of `gg`).
    Pending,
    Unbound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Keymap<A> {
    bindings: Vec<Binding<A>>,
    /// Actions the user's table set, whose defaults no longer apply.
    configured: Vec<A>,
}

impl<A: KeyAction> Keymap<A> {
    /// The keymap from a surface's `[<surface>.keys]` table and the shared
    /// `[keys]` table laid over `defaults`, and a message for each entry
    /// that could not be used. `check` rejects a binding this surface
    /// cannot use.
    ///
    /// The surface's own table decides an action it names. `[keys]` covers
    /// the actions it leaves out: an entry there for an action this surface
    /// does not have, or a key `check` rejects, is skipped quietly, since it
    /// may be meant for the other surface, and an entry left with no key
    /// that way keeps the defaults.
    pub(crate) fn build(
        surface: &str,
        tables: &KeyTables,
        defaults: &DefaultKeys<A>,
        check: impl Fn(&[KeyChord]) -> Result<(), String>,
    ) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut keymap = Self {
            bindings: Vec::new(),
            configured: Vec::new(),
        };
        let own_table = format!("{surface}.keys");
        if let Some(own) = &tables.own {
            keymap.add_user_table(&own_table, own, false, &check, &mut warnings);
        }
        if let Some(shared) = &tables.shared {
            keymap.add_user_table("keys", shared, true, &check, &mut warnings);
        }
        for (action, hinted, quiet) in defaults {
            if keymap.configured.contains(action) {
                continue;
            }
            let keys = hinted
                .iter()
                .map(|text| (text, true))
                .chain(quiet.iter().map(|text| (text, false)));
            for (text, hinted) in keys {
                let keys = parse_binding(text).expect("default keys parse");
                keymap.bind_unless_taken(keys, *action, hinted);
            }
        }
        (keymap, warnings)
    }

    /// Bind the entries of the user's table `table`. `shared` is the
    /// `[keys]` table, which yields to the surface's own table and to what
    /// this surface cannot use.
    fn add_user_table(
        &mut self,
        table: &str,
        entries: &toml::Table,
        shared: bool,
        check: &impl Fn(&[KeyChord]) -> Result<(), String>,
        warnings: &mut Vec<String>,
    ) {
        // Bindings from an earlier (more specific) table, which win.
        let earlier = self.bindings.len();
        for (name, value) in entries {
            let Some(action) = A::from_name(name) else {
                if !(shared && is_known_action(name)) {
                    warnings.push(format!("{table}: unknown action `{name}`"));
                }
                continue;
            };
            if self.configured.contains(&action) {
                continue;
            }
            let texts: Vec<&str> = match value {
                toml::Value::String(text) => vec![text.as_str()],
                toml::Value::Array(items) => {
                    let texts: Vec<&str> = items.iter().filter_map(toml::Value::as_str).collect();
                    if texts.len() != items.len() {
                        warnings.push(format!("{table}.{name}: keys must be strings"));
                    }
                    texts
                }
                _ => {
                    warnings.push(format!("{table}.{name}: expected a key or a list of keys"));
                    continue;
                }
            };
            let mut bound = Vec::new();
            for text in &texts {
                let keys = match parse_binding(text) {
                    Ok(keys) => keys,
                    Err(reason) => {
                        warnings.push(format!("{table}.{name}: {reason}"));
                        continue;
                    }
                };
                if let Err(reason) = check(&keys) {
                    if !shared {
                        warnings.push(format!("{table}.{name}: {reason}"));
                    }
                    continue;
                }
                let clashes_earlier = self.bindings[..earlier].iter().any(|binding| {
                    binding.keys.starts_with(&keys) || keys.starts_with(&binding.keys)
                });
                if clashes_earlier {
                    continue;
                }
                if let Some(other) = self.bindings[earlier..]
                    .iter()
                    .chain(&bound)
                    .find(|binding| binding.keys == keys)
                {
                    warnings.push(format!(
                        "{table}: `{text}` is bound to both {} and {name}",
                        other.action.name()
                    ));
                    continue;
                }
                bound.push(Binding {
                    keys,
                    action,
                    hinted: true,
                });
            }
            // An empty list unbinds; a shared entry whose keys all went to
            // the other surface or a more specific table keeps the defaults.
            if !shared || texts.is_empty() || !bound.is_empty() {
                self.configured.push(action);
            }
            self.bindings.extend(bound);
        }
    }

    /// Whether `keys` shares a key, or the start of one, with a binding, so
    /// that one would shadow the other.
    fn clashes(&self, keys: &[KeyChord]) -> bool {
        self.bindings
            .iter()
            .any(|binding| binding.keys.starts_with(keys) || keys.starts_with(&binding.keys))
    }

    /// Bind `keys` to `action` unless they would clash with a binding
    /// already made; earlier bindings win.
    pub(crate) fn bind_unless_taken(&mut self, keys: Vec<KeyChord>, action: A, hinted: bool) {
        if !self.clashes(&keys) {
            self.bindings.push(Binding {
                keys,
                action,
                hinted,
            });
        }
    }

    /// Bind `keys` to `action`, taking them from any binding they clash with.
    pub(crate) fn force_bind(&mut self, keys: Vec<KeyChord>, action: A) {
        self.bindings.retain(|binding| {
            !(binding.keys.starts_with(&keys) || keys.starts_with(&binding.keys))
        });
        self.bindings.push(Binding {
            keys,
            action,
            hinted: true,
        });
    }

    /// The user's table set `action`'s keys.
    pub(crate) fn is_configured(&self, action: A) -> bool {
        self.configured.contains(&action)
    }

    pub(crate) fn has_binding(&self, action: A) -> bool {
        self.bindings.iter().any(|binding| binding.action == action)
    }

    /// Some binding starts with `chord`.
    pub(crate) fn binds_first(&self, chord: KeyChord) -> bool {
        self.bindings.iter().any(|binding| binding.keys[0] == chord)
    }

    pub(crate) fn lookup(&self, pressed: &[KeyChord]) -> KeyLookup<A> {
        if let Some(binding) = self.bindings.iter().find(|binding| binding.keys == pressed) {
            return KeyLookup::Action(binding.action);
        }
        if self
            .bindings
            .iter()
            .any(|binding| binding.keys.len() > pressed.len() && binding.keys.starts_with(pressed))
        {
            return KeyLookup::Pending;
        }
        KeyLookup::Unbound
    }

    /// The labels a keymap help shows for `action`'s keys, such as `k`, `↑`.
    pub(crate) fn labels(&self, action: A) -> Vec<String> {
        self.bindings
            .iter()
            .filter(|binding| binding.action == action && binding.hinted)
            .map(|binding| binding_label(&binding.keys))
            .collect()
    }
}

/// Keys pressed so far toward a multi-key binding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PendingKeys(Vec<KeyChord>);

impl PendingKeys {
    /// Add `chord` to the pending sequence and return what it completes. A
    /// press that neither completes nor continues a binding drops the
    /// sequence and is tried on its own, as Vim does after `g`.
    pub(crate) fn resolve<A: Copy + PartialEq>(
        &mut self,
        chord: KeyChord,
        lookup: impl Fn(&[KeyChord]) -> KeyLookup<A>,
    ) -> KeyLookup<A> {
        self.0.push(chord);
        let mut result = lookup(&self.0);
        if result == KeyLookup::Unbound && self.0.len() > 1 {
            self.0 = vec![chord];
            result = lookup(&self.0);
        }
        if result != KeyLookup::Pending {
            self.0.clear();
        }
        result
    }

    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}

/// The user's key tables for one surface: its own `[<surface>.keys]` and
/// the shared `[keys]`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct KeyTables {
    pub(crate) own: Option<toml::Table>,
    pub(crate) shared: Option<toml::Table>,
}

impl KeyTables {
    /// The tables in the config text `text`, and a message for each that
    /// cannot be read (or for the whole file, when it is not TOML).
    pub(crate) fn from_config_text(text: &str, surface: &str) -> (Self, Vec<String>) {
        let config = match toml::from_str::<toml::Table>(text) {
            Ok(config) => config,
            Err(error) => {
                return (
                    Self::default(),
                    vec![format!(
                        "config.toml is not valid TOML: {}",
                        error.message()
                    )],
                );
            }
        };
        let mut warnings = Vec::new();
        let mut table = |value: Option<&toml::Value>, name: &str| match value {
            None => None,
            Some(toml::Value::Table(keys)) => Some(keys.clone()),
            Some(_) => {
                warnings.push(format!("{name} in config.toml must be a table"));
                None
            }
        };
        let own = table(
            config.get(surface).and_then(|table| table.get("keys")),
            &format!("{surface}.keys"),
        );
        let shared = table(config.get("keys"), "keys");
        (Self { own, shared }, warnings)
    }

    /// The tables in `~/.config/pmux/config.toml`; none when there is no
    /// such file.
    pub(crate) fn load(surface: &str) -> (Self, Vec<String>) {
        match read_config_text() {
            Some(text) => Self::from_config_text(&text, surface),
            None => (Self::default(), Vec::new()),
        }
    }
}

/// An action some surface has, so a `[keys]` entry for it is not a typo.
fn is_known_action(name: &str) -> bool {
    crate::tui::SidebarAction::from_name(name).is_some()
        || crate::attach_picker::PickerAction::from_name(name).is_some()
}

/// The text of `~/.config/pmux/config.toml`, or `None` when there is none.
fn read_config_text() -> Option<String> {
    let path = crate::config::pmux_config_toml_path().ok()?;
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<KeyChord> {
        parse_binding(text).unwrap()
    }

    #[test]
    fn bindings_parse_named_keys_modifiers_and_character_runs() {
        let ctrl_d = KeyChord {
            code: KeyCode::Char('d'),
            ctrl: true,
            alt: false,
        };
        for text in ["C-d", "<C-d>", "ctrl-d", "Ctrl+D"] {
            assert_eq!(keys(text), [ctrl_d], "{text}");
        }
        assert_eq!(keys("<CR>"), [KeyChord::plain(KeyCode::Enter)]);
        assert_eq!(keys("Enter"), [KeyChord::plain(KeyCode::Enter)]);
        assert_eq!(keys("pgdn"), [KeyChord::plain(KeyCode::PageDown)]);
        assert_eq!(keys("f5"), [KeyChord::plain(KeyCode::F(5))]);
        assert_eq!(
            keys("gg"),
            [KeyChord::plain(KeyCode::Char('g')); 2],
            "a run of characters is a sequence"
        );
        assert_eq!(keys("G"), [KeyChord::plain(KeyCode::Char('G'))]);
        assert_eq!(keys("z a").len(), 2);
        assert_eq!(keys("<C-w> j").len(), 2);
        assert!(parse_binding("<nope>").is_err());
        assert!(parse_binding("C-nope").is_err());
        assert!(parse_binding("  ").is_err());
    }

    #[test]
    fn only_plain_characters_are_typed() {
        assert_eq!(keys("j")[0].typed_char(), Some('j'));
        assert_eq!(keys("space")[0].typed_char(), Some(' '));
        assert_eq!(keys("C-j")[0].typed_char(), None);
        assert_eq!(keys("M-j")[0].typed_char(), None);
        assert_eq!(keys("enter")[0].typed_char(), None);
    }

    #[test]
    fn key_tables_must_be_tables_in_valid_toml() {
        let (tables, warnings) = KeyTables::from_config_text("", "picker");
        assert_eq!(tables, KeyTables::default());
        assert!(warnings.is_empty());
        let (tables, warnings) = KeyTables::from_config_text(
            "[keys]\nmove_down = \"C-j\"\n[picker.keys]\nselect = \"tab\"\n",
            "picker",
        );
        assert!(tables.own.is_some() && tables.shared.is_some());
        assert!(warnings.is_empty());
        let (_, warnings) = KeyTables::from_config_text("keys = 3\n[picker]\nkeys = 3\n", "picker");
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        let (tables, warnings) = KeyTables::from_config_text("not toml [", "picker");
        assert_eq!(tables, KeyTables::default());
        assert_eq!(warnings.len(), 1);
    }
}
