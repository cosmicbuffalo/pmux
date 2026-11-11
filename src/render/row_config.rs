//! Row layouts from `~/.config/pmux/config.toml`.
//!
//! `[rows]` sets the layout the sidebar and the picker share, and
//! `[sidebar.rows]` and `[picker.rows]` change it for one of them:
//!
//! ```toml
//! [rows]
//! preset = "pmux"
//! lines = [
//!   ["state_dot", "title", ">", "arrow"],
//!   ["agent", "cwd", "prompts", ">", "location"],
//! ]
//!
//! [rows.colors]
//! cwd = "#89b4fa"
//! prompts = 117
//! separator = "darkgray"
//!
//!
//! [rows.outline]
//! style = "rounded"
//! rule_detached = " ┄"
//! ```
//!
//! A table that sets `preset` or `lines` decides that surface's lines; `lines`
//! replaces the preset's. Colors add up, the surface's own over the shared
//! ones, and so do outline glyphs, except that a surface's own outline
//! `style` starts it over from that style. Anything pmux cannot use is
//! reported and left out, and the rest still applies.

use super::row::{ColorTarget, RowLayout, RowLine, RowOutline, RowToken};
use super::*;

/// Where rows are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowSurface {
    Sidebar,
    Picker,
}

impl RowSurface {
    fn table(self) -> &'static str {
        match self {
            RowSurface::Sidebar => "sidebar",
            RowSurface::Picker => "picker",
        }
    }
}

/// The marker in a line that pins the tokens after it to the right edge.
const RIGHT_MARKER: &str = ">";

/// The layout `surface` draws rows with, and what in the config could not be
/// used.
pub fn load_row_layout(surface: RowSurface) -> (RowLayout, Vec<String>) {
    let Ok(path) = crate::config::pmux_config_toml_path() else {
        return (RowLayout::pmux(), Vec::new());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => row_layout_from_config(&text, surface),
        Err(_) => (RowLayout::pmux(), Vec::new()),
    }
}

/// [`load_row_layout`] for the config text `text`.
pub fn row_layout_from_config(text: &str, surface: RowSurface) -> (RowLayout, Vec<String>) {
    let config = match toml::from_str::<toml::Table>(text) {
        Ok(config) => config,
        // The keymap reports a config that is not TOML at all.
        Err(_) => return (RowLayout::pmux(), Vec::new()),
    };
    let mut problems = Vec::new();
    let shared = rows_table(config.get("rows"), "rows", &mut problems);
    let own_name = format!("{}.rows", surface.table());
    let own = rows_table(
        config
            .get(surface.table())
            .and_then(|table| table.get("rows")),
        &own_name,
        &mut problems,
    );

    let decides = |table: Option<&toml::Table>| {
        table.is_some_and(|table| table.contains_key("preset") || table.contains_key("lines"))
    };
    let (lines_table, lines_name) = if decides(own) {
        (own, own_name.as_str())
    } else {
        (shared, "rows")
    };
    let mut layout = match lines_table {
        Some(table) => layout_from_table(table, lines_name, &mut problems),
        None => RowLayout::pmux(),
    };
    for (table, name) in [(shared, "rows"), (own, own_name.as_str())] {
        if let Some(colors) = table.and_then(|table| table.get("colors")) {
            read_colors(colors, name, &mut layout, &mut problems);
        }
        match table.and_then(|table| table.get("pills")) {
            None => {}
            Some(toml::Value::Boolean(pills)) => layout.pills = *pills,
            Some(_) => problems.push(format!("{name}.pills must be true or false")),
        }
    }
    let outline_table =
        |table: Option<&toml::Table>, name: &str, problems: &mut Vec<String>| match table
            .and_then(|table| table.get("outline"))
        {
            None => None,
            Some(toml::Value::Table(outline)) => Some(outline.clone()),
            Some(_) => {
                problems.push(format!("{name}.outline must be a table"));
                None
            }
        };
    let shared_outline = outline_table(shared, "rows", &mut problems);
    let own_outline = outline_table(own, &own_name, &mut problems);
    let restyles = own_outline
        .as_ref()
        .is_some_and(|outline| outline.contains_key("style"));
    let mut tables = vec![(own_outline, own_name.clone())];
    if !restyles {
        tables.insert(0, (shared_outline, "rows".to_string()));
    }
    for (outline, name) in tables {
        if let Some(outline) = outline {
            read_outline(
                &outline,
                &format!("{name}.outline"),
                &mut layout.outline,
                &mut problems,
            );
        }
    }
    (layout, problems)
}

/// The outline glyphs config can set, as named there.
const OUTLINE_GLYPHS: &[&str] = &[
    "top",
    "bottom",
    "side",
    "side_detached",
    "rule",
    "rule_detached",
];

/// Apply `table`'s `style`, then its glyphs, to `outline`.
fn read_outline(
    table: &toml::Table,
    name: &str,
    outline: &mut RowOutline,
    problems: &mut Vec<String>,
) {
    match table.get("style") {
        None => {}
        Some(toml::Value::String(style)) => match RowOutline::style(style) {
            Some(styled) => *outline = styled,
            None => problems.push(format!(
                "{name}.style: unknown style \"{style}\" (known: {})",
                RowOutline::STYLES.join(", ")
            )),
        },
        Some(_) => problems.push(format!("{name}.style must be a string")),
    }
    for (key, value) in table {
        if key == "style" {
            continue;
        }
        if !OUTLINE_GLYPHS.contains(&key.as_str()) {
            problems.push(format!("{name}: unknown setting \"{key}\""));
            continue;
        }
        let Some(glyph) = value.as_str() else {
            problems.push(format!("{name}.{key} must be a string"));
            continue;
        };
        let one_column = |c: char| crate::display_width(c.encode_utf8(&mut [0u8; 4])) == 1;
        let is_rule = key.starts_with("rule");
        let count = glyph.chars().count();
        let fits = glyph.chars().all(one_column)
            && if is_rule {
                (1..=8).contains(&count)
            } else {
                count == 1
            };
        if !fits {
            problems.push(format!(
                "{name}.{key}: {}",
                if is_rule {
                    "a rule is 1 to 8 single-column characters"
                } else {
                    "must be one single-column character"
                }
            ));
            continue;
        }
        let slot = match key.as_str() {
            "top" => &mut outline.top,
            "bottom" => &mut outline.bottom,
            "side" => &mut outline.side,
            "side_detached" => &mut outline.side_detached,
            "rule" => &mut outline.rule,
            _ => &mut outline.rule_detached,
        };
        *slot = glyph.to_string();
    }
}

/// The table at `value`, reporting anything else that is there.
fn rows_table<'a>(
    value: Option<&'a toml::Value>,
    name: &str,
    problems: &mut Vec<String>,
) -> Option<&'a toml::Table> {
    match value? {
        toml::Value::Table(table) => {
            for key in table.keys() {
                if !matches!(
                    key.as_str(),
                    "preset" | "lines" | "colors" | "outline" | "pills"
                ) {
                    problems.push(format!("{name}: unknown setting \"{key}\""));
                }
            }
            Some(table)
        }
        _ => {
            problems.push(format!("{name} must be a table"));
            None
        }
    }
}

/// The preset `table` names, with its `lines` in place of the preset's.
fn layout_from_table(table: &toml::Table, name: &str, problems: &mut Vec<String>) -> RowLayout {
    let mut layout = match table.get("preset") {
        None => RowLayout::pmux(),
        Some(toml::Value::String(preset)) => RowLayout::preset(preset).unwrap_or_else(|| {
            problems.push(format!(
                "{name}.preset: unknown preset \"{preset}\" (known: {})",
                preset_names()
            ));
            RowLayout::pmux()
        }),
        Some(_) => {
            problems.push(format!("{name}.preset must be a string"));
            RowLayout::pmux()
        }
    };
    if let Some(value) = table.get("lines")
        && let Some(lines) = read_lines(value, &format!("{name}.lines"), problems)
    {
        layout.lines = lines;
    }
    layout
}

/// The preset names, comma-separated.
fn preset_names() -> String {
    RowLayout::PRESETS
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The preset `[rows]` names in the config text `text`, if any.
pub fn configured_preset(text: &str) -> Option<String> {
    let config = toml::from_str::<toml::Table>(text).ok()?;
    let preset = config.get("rows")?.get("preset")?.as_str()?;
    Some(preset.to_string())
}

/// `text` with `[rows]` set to `preset`, the rest of it untouched: the
/// table's `preset` line is replaced, or added under its header, or the
/// table is added at the end.
pub fn with_preset(text: &str, preset: &str) -> Result<String, String> {
    if RowLayout::preset(preset).is_none() {
        return Err(format!(
            "unknown preset \"{preset}\" (known: {})",
            preset_names()
        ));
    }
    if let Err(error) = toml::from_str::<toml::Table>(text) {
        return Err(format!(
            "config.toml is not valid TOML: {}",
            error.message()
        ));
    }
    let setting = format!("preset = \"{preset}\"");
    let lines: Vec<&str> = text.lines().collect();
    let header = lines.iter().position(|line| line.trim() == "[rows]");
    let edited = match header {
        Some(header) => {
            let end = lines[header + 1..]
                .iter()
                .position(|line| line.trim_start().starts_with('['))
                .map_or(lines.len(), |offset| header + 1 + offset);
            let existing = (header + 1..end).find(|&index| {
                let line = lines[index].trim_start();
                line.strip_prefix("preset")
                    .is_some_and(|rest| rest.trim_start().starts_with('='))
            });
            let mut edited: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
            match existing {
                Some(index) => edited[index] = setting,
                None => edited.insert(header + 1, setting),
            }
            let mut text = edited.join("\n");
            text.push('\n');
            text
        }
        None => {
            let mut text = text.to_string();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            if !text.trim().is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("[rows]\n{setting}\n"));
            text
        }
    };
    // Check the result rather than trust the line edit: `rows` set some other
    // way (dotted keys, an inline table) is left for the user to change.
    if configured_preset(&edited).as_deref() != Some(preset) {
        return Err(
            "could not set the preset in config.toml; set `preset` under [rows] there".to_string(),
        );
    }
    Ok(edited)
}

/// The lines `value` lists, or `None` (reported) when it lists none pmux
/// can draw.
fn read_lines(value: &toml::Value, name: &str, problems: &mut Vec<String>) -> Option<Vec<RowLine>> {
    let Some(items) = value.as_array() else {
        problems.push(format!("{name} must be a list of lines"));
        return None;
    };
    let mut lines = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let line_name = format!("{name}[{}]", index + 1);
        let Some(words) = item.as_array() else {
            problems.push(format!("{line_name} must be a list of token names"));
            continue;
        };
        let mut left = Vec::new();
        let mut right = Vec::new();
        let mut pinned = false;
        for word in words {
            let Some(word) = word.as_str() else {
                problems.push(format!("{line_name}: token names are strings"));
                continue;
            };
            if word == RIGHT_MARKER {
                if pinned {
                    problems.push(format!("{line_name}: more than one \"{RIGHT_MARKER}\""));
                }
                pinned = true;
                continue;
            }
            match RowToken::from_name(word) {
                Some(token) if pinned => right.push(token),
                Some(token) => left.push(token),
                None => problems.push(format!("{line_name}: unknown token \"{word}\"")),
            }
        }
        if !left.is_empty() || !right.is_empty() {
            lines.push(RowLine::new(&left, &right));
        }
    }
    if lines.is_empty() {
        problems.push(format!("{name} has no tokens to show; using the preset's"));
        return None;
    }
    Some(lines)
}

fn read_colors(
    value: &toml::Value,
    name: &str,
    layout: &mut RowLayout,
    problems: &mut Vec<String>,
) {
    let Some(colors) = value.as_table() else {
        problems.push(format!("{name}.colors must be a table"));
        return;
    };
    for (key, value) in colors {
        let target = if key == "separator" {
            Some(ColorTarget::Separator)
        } else {
            RowToken::from_name(key).map(ColorTarget::Token)
        };
        let Some(target) = target else {
            problems.push(format!("{name}.colors: unknown token \"{key}\""));
            continue;
        };
        match parse_color(value) {
            Ok(color) => {
                layout.colors.insert(target, color);
            }
            Err(reason) => problems.push(format!("{name}.colors.{key}: {reason}")),
        }
    }
}

/// A color as config writes it: a terminal color name (`cyan`,
/// `lightblue`, `darkgray`, `default`), a 256-color index (`117`), or hex
/// (`#89b4fa`, `#8bf`).
pub fn parse_color(value: &toml::Value) -> Result<Color, String> {
    let text = match value {
        toml::Value::Integer(index) => {
            return u8::try_from(*index)
                .map(Color::Indexed)
                .map_err(|_| format!("{index} is not a color index from 0 to 255"));
        }
        toml::Value::String(text) => text.trim(),
        _ => return Err("a color is a name, a number from 0 to 255, or #rrggbb".to_string()),
    };
    if let Some(hex) = text.strip_prefix('#') {
        let digits: Vec<u8> = hex
            .chars()
            .map(|digit| digit.to_digit(16).map(|value| value as u8))
            .collect::<Option<_>>()
            .ok_or_else(|| format!("\"{text}\" is not a hex color"))?;
        return match digits.as_slice() {
            [r, g, b] => Ok(Color::Rgb(r * 17, g * 17, b * 17)),
            [r1, r2, g1, g2, b1, b2] => Ok(Color::Rgb(r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2)),
            _ => Err(format!("\"{text}\" is not a hex color")),
        };
    }
    if let Ok(index) = text.parse::<u8>() {
        return Ok(Color::Indexed(index));
    }
    let name = text.to_ascii_lowercase().replace(['_', '-', ' '], "");
    let color = match name.as_str() {
        "default" | "reset" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "white" => Color::White,
        _ => return Err(format!("unknown color \"{text}\"")),
    };
    Ok(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(text: &str, surface: RowSurface) -> (RowLayout, Vec<String>) {
        row_layout_from_config(text, surface)
    }

    #[test]
    fn no_rows_config_draws_pmux_rows_on_both_surfaces() {
        for surface in [RowSurface::Sidebar, RowSurface::Picker] {
            assert_eq!(layout("", surface), (RowLayout::pmux(), Vec::new()));
            assert_eq!(
                layout("[restore]\nenabled = true\n", surface),
                (RowLayout::pmux(), Vec::new())
            );
        }
    }

    #[test]
    fn lines_replace_the_presets_and_a_marker_pins_tokens_right() {
        use RowToken::*;
        let text = r#"
            [rows]
            lines = [["state_dot", "title", ">", "location"], ["status", "age"]]
        "#;
        let (layout, problems) = layout(text, RowSurface::Sidebar);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            layout.lines,
            vec![
                RowLine::new(&[StateDot, Title], &[Location]),
                RowLine::new(&[Status, Age], &[]),
            ]
        );
    }

    #[test]
    fn rows_are_shared_and_a_surface_table_overrides_them() {
        use RowToken::*;
        let text = r#"
            [rows]
            lines = [["state_dot", "title"]]
            [rows.colors]
            cwd = "cyan"
            prompts = "red"

            [picker.rows]
            lines = [["title"], ["status"]]
            [picker.rows.colors]
            prompts = 117
        "#;
        let (sidebar, problems) = layout(text, RowSurface::Sidebar);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(sidebar.lines, vec![RowLine::new(&[StateDot, Title], &[])]);
        assert_eq!(sidebar.colors[&ColorTarget::Token(Prompts)], Color::Red);

        let (picker, problems) = layout(text, RowSurface::Picker);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            picker.lines,
            vec![RowLine::new(&[Title], &[]), RowLine::new(&[Status], &[])]
        );
        assert_eq!(picker.colors[&ColorTarget::Token(Cwd)], Color::Cyan);
        assert_eq!(
            picker.colors[&ColorTarget::Token(Prompts)],
            Color::Indexed(117)
        );
    }

    #[test]
    fn a_surface_preset_wins_over_shared_lines() {
        let text = r#"
            [rows]
            lines = [["title"]]
            [sidebar.rows]
            preset = "pmux"
        "#;
        assert_eq!(layout(text, RowSurface::Sidebar).0, RowLayout::pmux());
        assert_ne!(layout(text, RowSurface::Picker).0, RowLayout::pmux());
    }

    #[test]
    fn mistakes_are_reported_and_the_rest_still_applies() {
        use RowToken::*;
        let text = r##"
            [rows]
            preset = "fancy"
            lines = [["title", "nonsense", ">", "arrow", ">"], [], ["status"]]
            shape = "round"
            [rows.colors]
            cwd = "#12345"
            title = 300
            sparkle = "red"
            age = "lightgreen"
        "##;
        let (layout, problems) = layout(text, RowSurface::Sidebar);
        assert_eq!(
            layout.lines,
            vec![
                RowLine::new(&[Title], &[Arrow]),
                RowLine::new(&[Status], &[]),
            ]
        );
        assert_eq!(layout.colors.len(), 1);
        assert_eq!(layout.colors[&ColorTarget::Token(Age)], Color::LightGreen);
        let expected = [
            "rows: unknown setting \"shape\"",
            "rows.preset: unknown preset \"fancy\" (known: pmux, compact, herdr)",
            "rows.lines[1]: unknown token \"nonsense\"",
            "rows.lines[1]: more than one \">\"",
            "rows.colors.cwd: \"#12345\" is not a hex color",
            "rows.colors: unknown token \"sparkle\"",
            "rows.colors.title: 300 is not a color index from 0 to 255",
        ];
        for message in expected {
            assert!(
                problems.iter().any(|problem| problem == message),
                "{message}: {problems:?}"
            );
        }
        assert_eq!(problems.len(), expected.len(), "{problems:?}");
    }

    #[test]
    fn lines_with_nothing_to_show_keep_the_presets() {
        let (layout, problems) = layout("[rows]\nlines = [[\"nope\"]]\n", RowSurface::Sidebar);
        assert_eq!(layout, RowLayout::pmux());
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("no tokens to show"))
        );
    }

    #[test]
    fn outlines_take_a_style_and_glyphs_and_a_surface_style_starts_over() {
        let text = r#"
            [rows.outline]
            style = "rounded"
            rule_detached = " ┄"

            [picker.rows.outline]
            style = "heavy"
        "#;
        let (sidebar, problems) = layout(text, RowSurface::Sidebar);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(sidebar.outline.top, "╭");
        assert_eq!(sidebar.outline.rule_detached, " ┄");
        let (picker, _) = layout(text, RowSurface::Picker);
        assert_eq!(picker.outline, RowOutline::style("heavy").unwrap());

        let text = r#"
            [rows.outline]
            side = "▏"
            [sidebar.rows.outline]
            top = "▛"
        "#;
        let (sidebar, problems) = layout(text, RowSurface::Sidebar);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            (sidebar.outline.top.as_str(), sidebar.outline.side.as_str()),
            ("▛", "▏")
        );
    }

    #[test]
    fn outline_mistakes_are_reported() {
        let text = r#"
            [rows.outline]
            style = "wavy"
            top = "┌┌"
            side = "界"
            rule = ""
            corner = "+"
            bottom = 1
        "#;
        let (layout, problems) = layout(text, RowSurface::Sidebar);
        assert_eq!(layout.outline, RowOutline::default());
        let expected = [
            "rows.outline.style: unknown style \"wavy\" (known: box, rounded, heavy, ascii, none)",
            "rows.outline.top: must be one single-column character",
            "rows.outline.side: must be one single-column character",
            "rows.outline.rule: a rule is 1 to 8 single-column characters",
            "rows.outline: unknown setting \"corner\"",
            "rows.outline.bottom must be a string",
        ];
        for message in expected {
            assert!(
                problems.iter().any(|problem| problem == message),
                "{message}: {problems:?}"
            );
        }
        assert_eq!(problems.len(), expected.len(), "{problems:?}");
    }

    #[test]
    fn pills_follow_the_preset_unless_config_sets_them() {
        let (herdr, _) = layout("[rows]\npreset = \"herdr\"\n", RowSurface::Sidebar);
        assert!(!herdr.pills);
        let (pmux, _) = layout("", RowSurface::Sidebar);
        assert!(pmux.pills);

        let (herdr_pills, problems) = layout(
            "[rows]\npreset = \"herdr\"\npills = true\n",
            RowSurface::Sidebar,
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert!(herdr_pills.pills);

        // A surface's own setting wins over the shared one.
        let text = "[rows]\npills = false\n[picker.rows]\npills = true\n";
        assert!(!layout(text, RowSurface::Sidebar).0.pills);
        assert!(layout(text, RowSurface::Picker).0.pills);
    }

    #[test]
    fn pills_must_be_a_boolean() {
        let (layout, problems) = layout("[rows]\npills = \"no\"\n", RowSurface::Sidebar);
        assert!(layout.pills);
        assert_eq!(
            problems,
            vec!["rows.pills must be true or false".to_string()]
        );
    }

    #[test]
    fn a_preset_sets_lines_and_outline() {
        let (herdr, problems) = layout("[rows]\npreset = \"herdr\"\n", RowSurface::Sidebar);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(herdr, RowLayout::preset("herdr").unwrap());
        assert_eq!(herdr.outline, RowOutline::style("none").unwrap());
        // Outline settings still apply on top of a preset's.
        let text = "[rows]\npreset = \"herdr\"\n[rows.outline]\nside = \"▏\"\n";
        assert_eq!(layout(text, RowSurface::Picker).0.outline.side, "▏");
        for (name, _) in RowLayout::PRESETS {
            assert!(RowLayout::preset(name).is_some(), "{name}");
        }
    }

    #[test]
    fn setting_a_preset_keeps_the_rest_of_the_config() {
        // No [rows] table yet: one is added at the end.
        let text = "# my config\n[restore]\nenabled = true\n";
        let edited = with_preset(text, "compact").unwrap();
        assert_eq!(
            edited,
            "# my config\n[restore]\nenabled = true\n\n[rows]\npreset = \"compact\"\n"
        );
        // The preset line is replaced in place, comments and all else kept.
        let edited = with_preset(&edited, "herdr").unwrap();
        assert!(edited.contains("[rows]\npreset = \"herdr\"\n"));
        assert!(edited.starts_with("# my config\n[restore]\nenabled = true\n"));
        // A [rows] table without a preset gains one under its header.
        let text = "[rows]\nlines = [[\"title\"]]\n\n[rows.colors]\ncwd = \"cyan\"\n";
        let edited = with_preset(text, "pmux").unwrap();
        assert_eq!(configured_preset(&edited).as_deref(), Some("pmux"));
        assert!(edited.contains("[rows]\npreset = \"pmux\"\nlines"));
        assert!(edited.contains("[rows.colors]\ncwd = \"cyan\""));
        assert_eq!(
            with_preset("", "pmux").unwrap(),
            "[rows]\npreset = \"pmux\"\n"
        );

        assert!(
            with_preset("", "fancy")
                .unwrap_err()
                .contains("unknown preset")
        );
        assert!(
            with_preset("[rows", "pmux")
                .unwrap_err()
                .contains("not valid TOML")
        );
        let dotted = "rows.preset = \"pmux\"\n";
        assert!(with_preset(dotted, "herdr").is_err());
    }

    /// The examples in docs/sidebar-customization.md: `(name, toml)`, each
    /// a toml block after an `<!-- example: NAME -->` line.
    fn customization_doc_examples() -> Vec<(&'static str, &'static str)> {
        let doc = include_str!("../../docs/sidebar-customization.md");
        doc.split("<!-- example: ")
            .skip(1)
            .map(|part| {
                let (name, rest) = part.split_once(" -->\n```toml\n").expect("a toml block");
                let (toml, _) = rest.split_once("```").expect("a closed block");
                (name, toml)
            })
            .collect()
    }

    #[test]
    fn every_customization_doc_example_applies_cleanly_and_has_its_screenshot() {
        let examples = customization_doc_examples();
        assert!(examples.len() >= 9, "{examples:?}");
        let doc = include_str!("../../docs/sidebar-customization.md");
        for (name, toml) in examples {
            for surface in [RowSurface::Sidebar, RowSurface::Picker] {
                let (_, problems) = row_layout_from_config(toml, surface);
                assert!(problems.is_empty(), "{name}: {problems:?}");
            }
            let image = format!("images/customization/{name}.png");
            assert!(
                doc.contains(&format!("]({image})")),
                "{name}: no screenshot link"
            );
            assert!(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("docs")
                    .join(&image)
                    .exists(),
                "{name}: run `python3 scripts/screenshots.py customization/{name}`"
            );
        }
    }

    #[test]
    fn colors_are_names_indexes_or_hex() {
        let color = |text: &str| parse_color(&toml::Value::String(text.to_string()));
        assert_eq!(color("cyan"), Ok(Color::Cyan));
        assert_eq!(color("Light_Blue"), Ok(Color::LightBlue));
        assert_eq!(color("dark-grey"), Ok(Color::DarkGray));
        assert_eq!(color("default"), Ok(Color::Reset));
        assert_eq!(color("117"), Ok(Color::Indexed(117)));
        assert_eq!(color("#89b4fa"), Ok(Color::Rgb(0x89, 0xb4, 0xfa)));
        assert_eq!(color("#8bf"), Ok(Color::Rgb(0x88, 0xbb, 0xff)));
        assert_eq!(
            parse_color(&toml::Value::Integer(42)),
            Ok(Color::Indexed(42))
        );
        assert!(color("#xyz").is_err());
        assert!(color("chartreuse").is_err());
        assert!(parse_color(&toml::Value::Integer(-1)).is_err());
        assert!(parse_color(&toml::Value::Boolean(true)).is_err());
    }
}
